package com.example.personnel.service;

import java.time.LocalDate;
import java.util.List;
import java.util.NoSuchElementException;

import org.springframework.stereotype.Service;
import org.springframework.data.domain.Page;
import org.springframework.data.domain.Pageable;
import org.springframework.stereotype.Service;

import com.example.personnel.model.dto.EmployeeIn;
import com.example.personnel.model.dto.HistoryEntry;
import com.example.personnel.model.entity.Company;
import com.example.personnel.model.entity.Employee;
import com.example.personnel.model.entity.SalaryReview;
import com.example.personnel.repository.CompaniesRepository;
import com.example.personnel.repository.EmployeeRepository;
import com.example.personnel.model.dto.EmployeeOut;
import com.example.personnel.model.dto.RdfRepresentation;
import com.example.personnel.model.filter.EmployeeSearch;
import org.graphrd.springdata.GraphRdSpecifications;
import com.example.personnel.repository.ReviewsRepository;
import org.graphrd.RdfFormat;
import org.graphrd.rdf.EntityQuery.Op;
import org.graphrd.rdf.RdfEntityManager;

import lombok.RequiredArgsConstructor;

/** Employees: validation, raises with a record kept, paged listing. */
@Service
@RequiredArgsConstructor
public class EmployeeService {

    /** A raise above this share of the salary needs a human, not this API. */
    private static final double MAX_RAISE_SHARE = 0.25;

    private final EmployeeRepository employees;
    private final CompaniesRepository companies;
    private final ReviewsRepository reviews;
    private final RdfEntityManager em; // writes entities out as RDF

    public EmployeeOut create(EmployeeIn in) {
        if (in.name() == null || in.name().isBlank()) {
            throw new IllegalArgumentException("the name is required");
        }
        if (employees.existsByEmail(in.email())) {
            throw new IllegalArgumentException("email already in use: " + in.email());
        }
        Company company = companies.findById(in.company()).orElse(null);
        if (company == null) {
            throw new IllegalArgumentException("unknown company: " + in.company());
        }
        Employee e = new Employee(null, in.name(), in.email(), in.age(), in.salary(),
                in.hired() == null ? LocalDate.now().toString() : in.hired(), company);
        employees.save(e); // the library mints the IRI and fills the id field
        return EmployeeOut.of(e);
    }

    public EmployeeOut get(String id) {
        return EmployeeOut.of(employees.findById(id)
                .orElseThrow(() -> new NoSuchElementException("no such employee: " + id)));
    }

    /** Raises the salary and writes a review record; history is a query away. */
    public EmployeeOut raise(String id, int amount, String reason) {
        Employee e = employees.findById(id)
                .orElseThrow(() -> new NoSuchElementException("no such employee: " + id));
        if (amount <= 0) {
            throw new IllegalArgumentException("the raise must be positive");
        }
        if (amount > e.getSalary() * MAX_RAISE_SHARE) {
            throw new IllegalArgumentException(
                    "a raise above " + (int) (MAX_RAISE_SHARE * 100) + "% of the salary needs HR");
        }
        int old = e.getSalary();
        e.setSalary(old + amount);
        employees.save(e);
        reviews.save(new SalaryReview(
                null, // minted by the library
                e, LocalDate.now().toString(), old, old + amount,
                reason == null || reason.isBlank() ? "annual review" : reason));
        return EmployeeOut.of(e);
    }

    /** The raise history of one employee, newest first. */
    public List<HistoryEntry> history(String id) {
        get(id); // 404 when the employee does not exist
        return reviews.findByEmployeeOrderByDateDesc(id).stream()
                .map(r -> new HistoryEntry(r.getDate(), r.getOldSalary(), r.getNewSalary(),
                        r.getNewSalary() - r.getOldSalary(), r.getReason()))
                .toList();
    }

    /** Every employee as its outbound shape — for listings and tests. */
    public List<EmployeeOut> pageAll() {
        return employees.findAll().stream().map(EmployeeOut::of).toList();
    }

    /** The repository, exposed for tests that clean up after themselves. */
    com.example.personnel.repository.EmployeeRepository repository() {
        return employees;
    }

    /** Paged listing with free-form filters — a specification, run by the repository. */
    public Page<EmployeeOut> search(EmployeeSearch s, Pageable pageable) {
        var spec = GraphRdSpecifications.<Employee>all();
        if (s.minAge() != null && s.maxAge() != null) {
            spec = spec.and(GraphRdSpecifications.between(Employee::getAge, s.minAge(), s.maxAge()));
        } else {
            if (s.minAge() != null) {
                spec = spec.and(GraphRdSpecifications.greaterThanEqual(Employee::getAge, s.minAge()));
            }
            if (s.maxAge() != null) {
                spec = spec.and(GraphRdSpecifications.lessThanEqual(Employee::getAge, s.maxAge()));
            }
        }
        if (s.nameStarts() != null && !s.nameStarts().isBlank()) {
            spec = spec.and(GraphRdSpecifications.ilike(Employee::getName, s.nameStarts() + "%"));
        }
        if (s.company() != null && !s.company().isBlank()) {
            spec = spec.and(GraphRdSpecifications.ref("worksFor", s.company()));
        }
        return employees.findAll(spec, pageable).map(EmployeeOut::of);
    }

    /** One employee as RDF in the format the Accept header asks for. */
    public RdfRepresentation oneAsRdf(String id, String accept) {
        RdfFormat format = formatOf(accept);
        return new RdfRepresentation(format.mediaType, em.toRdf(employees.findById(id).orElseThrow(() ->
                new NoSuchElementException("no such employee: " + id)), format));
    }

    /** The whole listing as one RDF document in the asked format. */
    public RdfRepresentation listAsRdf(String accept) {
        RdfFormat format = formatOf(accept);
        return new RdfRepresentation(format.mediaType, em.toRdf(employees.findAll(), format));
    }

    private static RdfFormat formatOf(String accept) {
        RdfFormat format = RdfFormat.of(accept);
        return format == null ? RdfFormat.JSONLD : format;
    }

    public void delete(String id) {
        get(id);
        employees.deleteById(id);
    }
}
