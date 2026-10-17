package com.example.personnel.repository;

import java.util.Optional;

import com.example.personnel.model.entity.Company;
import com.graphrd.springdata.GraphRdRepository;

/** Companies, as a plain Spring Data repository. */
public interface CompaniesRepository extends GraphRdRepository<Company, String> {

    Optional<Company> findByName(String name);
}
