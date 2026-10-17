package com.example.personnel.repository;

import java.util.List;
import java.util.Optional;

import org.springframework.data.domain.Page;
import org.springframework.data.domain.Pageable;

import com.example.personnel.model.dto.PayrollRow;
import com.example.personnel.model.dto.PayrollStats;
import com.example.personnel.model.entity.Employee;
import org.graphrd.springdata.GraphRdRepository;
import org.graphrd.springdata.Query;

/**
 * The one repository for employees: standard CRUD from
 * {@link GraphRdRepository}, derived names, {@code @Query} aggregates — and
 * dynamic searches run as specifications the repository already knows
 * ({@code findAll(spec, pageable)}), so nothing custom is ever written.
 */
public interface EmployeeRepository extends GraphRdRepository<Employee, String> {

    Optional<Employee> findByEmail(String email);

    boolean existsByEmail(String email);

    /** Everyone at one company — an equality on a reference field, by IRI. */
    List<Employee> findByWorksFor(String companyIri);

    /** Per company: headcount, payroll, average, spread — grouped and sorted by the server. */
    @Query("PREFIX ex: <{ns}> " + "SELECT ?company (COUNT(?e) AS ?heads)"
            + " (SUM(?s) AS ?total) (AVG(?s) AS ?average) (MIN(?s) AS ?lowest) (MAX(?s) AS ?highest)"
            + " WHERE { ?e a ex:Employee ; ex:salary ?s ; ex:worksFor ?c . ?c ex:name ?company }"
            + " GROUP BY ?company ORDER BY DESC(?total)")
    List<PayrollRow> payroll();

    /** The whole organization in one row. */
    @Query("PREFIX ex: <{ns}> " + "SELECT (COUNT(?e) AS ?employees)"
            + " (SUM(?s) AS ?payroll) (AVG(?s) AS ?average)"
            + " WHERE { ?e a ex:Employee ; ex:salary ?s }")
    PayrollStats payrollStats();

    /** What the raises on record have cost so far — a join across the review records. */
    @Query("PREFIX ex: <{ns}> " + "SELECT (SUM(?new - ?old) AS ?cost)"
            + " WHERE { ?r a ex:SalaryReview ; ex:newSalary ?new ; ex:oldSalary ?old }")
    long raiseCost();

    /** One company's row, by its name — a parameterized @Query (:company binds as a literal). */
    @Query("PREFIX ex: <{ns}> " + "SELECT ?company (COUNT(?e) AS ?heads)"
            + " (SUM(?s) AS ?total) (AVG(?s) AS ?average) (MIN(?s) AS ?lowest) (MAX(?s) AS ?highest)"
            + " WHERE { ?e a ex:Employee ; ex:salary ?s ; ex:worksFor ?c . ?c ex:name ?company"
            + " . FILTER(?company = :company) } GROUP BY ?company")
    PayrollRow companyPayroll(String company);

    /** The raise cost since a date (:since binds as a literal, compared as a string). */
    @Query("PREFIX ex: <{ns}> " + "SELECT (SUM(?new - ?old) AS ?cost)"
            + " WHERE { ?r a ex:SalaryReview ; ex:newSalary ?new ; ex:oldSalary ?old ; ex:date ?date"
            + " . FILTER(?date >= :since) }")
    long raiseCostSince(String since);
}
