package com.example.personnel.service;

import java.util.List;

import org.springframework.stereotype.Service;

import com.example.personnel.model.dto.CompanyIn;
import com.example.personnel.model.entity.Company;
import com.example.personnel.repository.CompaniesRepository;
import com.graphrd.rdf.RdfEntityManager;

import lombok.RequiredArgsConstructor;

/** Companies: what little there is to do — the controller stays a shell. */
@Service
@RequiredArgsConstructor
public class CompanyService {

    private final CompaniesRepository companies;
    private final RdfEntityManager em; // its namespace shapes the minted IRI

    /** Every company. */
    public List<Company> all() {
        return companies.findAll();
    }

    /** The repository, exposed for tests that clean up after themselves. */
    com.example.personnel.repository.CompaniesRepository repository() {
        return companies;
    }

    /** Creates a company; the IRI is derived from the name in the namespace. */
    public Company create(CompanyIn in) {
        if (in.name() == null || in.name().isBlank()) {
            throw new IllegalArgumentException("the name is required");
        }
        String id = em.namespace() + "company/" + in.name().toLowerCase().replaceAll("[^a-z0-9]+", "-");
        Company c = new Company(id, in.name(), in.industry());
        companies.save(c);
        return c;
    }
}
